"""Crash/rebind acceptance for one preconfigured, disposable API 35 assistant AVD. No audio."""

import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import time

from keystore_device_test import TARGET, TEST, build, check_result, require_preconfigured_apk, require_setup_complete


AVD = "eon-profile-death"
SERVICE = TARGET + "/.EonVoiceInteractionService"
OBSERVER = TEST + "/" + TARGET + ".ProfileActionAcceptance"


def require(ok, message):
    if not ok:
        raise SystemExit(message)


def select_device(sdk, serial):
    require(re.fullmatch(r"emulator-[0-9]+", serial), "Specify a local emulator serial.")
    require(not (os.environ.get("ADB_SERVER_SOCKET") or os.environ.get("ANDROID_ADB_SERVER_ADDRESS")),
            "Use a local adb server, not a socket or address override.")
    port = os.environ.get("ANDROID_ADB_SERVER_PORT", "5037")
    require(re.fullmatch(r"[0-9]{1,5}", port) and 0 < int(port) <= 65535, "Invalid local adb server port.")

    def command(*args, timeout=15):
        return subprocess.check_output([sdk / "platform-tools/adb", "-H", "127.0.0.1", "-P", port, *args],
                                       text=True, timeout=timeout, stderr=subprocess.STDOUT)

    rows = [line.split() for line in command("devices", "-l").splitlines() if line.split()[:1] == [serial]]
    require(len(rows) == 1 and rows[0][1:2] == ["device"], "Expected exactly one online selected emulator.")
    ids = [s.removeprefix("transport_id:") for s in rows[0] if s.startswith("transport_id:")]
    require(len(ids) == 1 and re.fullmatch(r"[1-9][0-9]*", ids[0]), "Missing unique adb transport ID.")
    require(command("-s", serial, "emu", "avd", "name").splitlines() == [AVD, "OK"],
            f"Refusing any AVD other than the disposable {AVD}.")

    # A disconnected/replaced emulator at the same console port is not our transport.
    def adb(*args, **kwargs):
        return command("-t", ids[0], *args, **kwargs)

    require(adb("get-serialno").strip() == serial, "Selected transport changed.")
    return adb


def check_device(adb):
    require(adb("shell", "id", "-u").strip() == "2000" and adb("shell", "getenforce").strip() == "Enforcing",
            "Use an enforcing emulator with ordinary adb shell, not adb root.")
    for prop, value in (("ro.boot.qemu.avd_name", AVD), ("ro.build.version.sdk", "35"), ("ro.hardware", "ranchu"),
                        ("ro.product.cpu.abi", "x86_64"), ("ro.build.id", "AE3A.240806.043"),
                        ("ro.build.version.incremental", "12960925")):
        require(adb("shell", "getprop", prop).strip() == value, "Expected the documented stock API 35 image.")
    require_setup_complete(adb)


def check_owner(adb):
    require(adb("shell", "am", "get-current-user").strip() == "0", "Expected the AVD owner user.")
    require_preconfigured_apk(adb)
    uid = adb("shell", "run-as", TARGET, "id", "-u").strip()
    require(re.fullmatch(r"1[0-9]{4}", uid), "Expected a debuggable owner-user app.")
    require(adb("shell", "pm", "list", "packages", "-U", "--user", "0", "--uid", uid).splitlines() ==
            [f"package:{TARGET} uid:{uid}"], "Refusing a shared or mismatched app UID.")
    require(adb("shell", "cmd", "role", "get-role-holders", "--user", "0", "android.app.role.ASSISTANT")
            .splitlines() == [TARGET], "EON is not the selected assistant.")
    return int(uid)


def bound(adb, *, shown):
    # Filter on-device: no show arguments, tokens, activity tree or other window contents.
    fields = adb("shell", "dumpsys voiceinteraction | grep -E "
                 "'^ *(mUser=|mComponent=|mDisabledShowContext=|mBound=[^ ]+ mService=|mShown=)' || :")
    lines = [s.strip() for s in fields.splitlines()]
    expected = ["mUser=0", "mComponent=" + SERVICE, "mDisabledShowContext=3"]
    connected = [s for s in lines if re.fullmatch(r"mBound=true mService=[\w.$]+@[0-9a-f]+", s)]
    if len(connected) != 1:
        return False
    expected += connected
    # On teardown there may be no active session connection at all.
    if shown or "mShown=false" in lines:
        expected.append("mShown=" + ("true" if shown else "false"))
    return sorted(lines) == sorted(expected)


def service_pid(adb, uid):
    field = adb("shell", f"dumpsys activity services {SERVICE} | grep -E '^ *app=' || :").strip()
    match = re.fullmatch(r"app=ProcessRecord\{[0-9a-f]+ ([1-9][0-9]*):" + re.escape(TARGET) +
                         rf"/u0a{uid - 10000}\}}", field)
    return int(match[1]) if match else None


def crash_record(adb, uid, pid):
    # API 35 ApplicationExitInfo.dump: adjacent identity and reason lines from the SAME record.
    # Export only a fixed marker; never read descriptions, stack traces or logs.
    fields = (f"/^ *timestamp=[^ ]+ [^ ]+ pid={pid} realUid={uid} packageUid={uid} definingUid=-?[0-9]+ user=0$/"
              r"{n; s/^ *process=com\.eon\.assistant reason=4 \(APP CRASH\(EXCEPTION\)\) subreason=.*/crash/p;}")
    markers = adb("shell", f"dumpsys activity exit-info {TARGET} | sed -nE {shlex.quote(fields)}").splitlines()
    require(markers in ([], ["crash"]), "Ambiguous scoped crash record.")
    return bool(markers)


def observe(adb, phase, expected):
    out = adb("shell", "am", "instrument", "-w", "-r", "--user", "0", "-e", "deathPhase", phase, OBSERVER,
              timeout=120)
    check_result(out, "profileDeath", expected)


def replacement(adb, uid, original):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        new = service_pid(adb, uid)
        if (new and new != original and bound(adb, shown=False) and crash_record(adb, uid, original)
                and time.monotonic() <= deadline):
            return new
        time.sleep(0.25)
    raise SystemExit("No corroborated old-process crash, dismissed session and automatic selected-service rebind.")


def run(adb, observer_apk):
    check_device(adb)
    uid = check_owner(adb)
    require(adb("shell", "pm", "list", "packages", "-u", "--user", "all", TARGET).splitlines() ==
            ["package:" + TARGET], "Install only the matching target; refusing to replace an existing observer.")
    original = service_pid(adb, uid)
    require(original and bound(adb, shown=False), "Expected a live selected service with collection closed, no card.")
    require(not crash_record(adb, uid, original), "Ambiguous prior crash for the original PID.")
    require("Success" in adb("install", "--user", "0", "-t", observer_apk, timeout=120).splitlines(),
            "Observer installation failed.")
    try:
        observe(adb, "arm", "ARMED-visible-action")
        require(check_owner(adb) == uid and service_pid(adb, uid) == original and bound(adb, shown=True),
                "Original service, selected owner, mask or visible session changed; not crashing.")
        # Stock ActivityManager addresses the package's ProcessRecord in user 0, NOT a numeric
        # PID looked up earlier. Never force-stop, signal a PID, retry a crash, or launch to rebind.
        print("Authorized result visible; requesting one package-scoped crash.", flush=True)
        require(not adb("shell", "am", "crash", "--user", "0", TARGET).strip(), "Android refused the crash.")
        new = replacement(adb, uid, original)
        require(check_owner(adb) == uid, "Rebound package identity changed.")
        print("Old crash and automatic rebind observed; checking for stale consent.", flush=True)
        observe(adb, "recover", "PASS-fresh-consent")
        require(service_pid(adb, uid) == new and bound(adb, shown=False), "Replacement binding changed.")
    finally:
        require("Success" in adb("uninstall", TEST).splitlines(), "Could not remove the observer.")
    print("profileDeath=PASS-crash-rebind-fresh-consent")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--disposable-serial", metavar="emulator-N", required=True)
    parser.add_argument("--crash-selected-assistant", action="store_true", required=True,
                        help=f"allow one stock package-scoped crash on the local {AVD} AVD only")
    return parser.parse_args(argv)


if __name__ == "__main__":
    args = parse_args()
    sdk = Path(os.environ.get("ANDROID_HOME") or os.environ["ANDROID_SDK_ROOT"])
    try:
        run(select_device(sdk, args.disposable_serial), build(sdk))
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
        raise SystemExit("Build or adb command failed or timed out. No automatic retry.") from None
