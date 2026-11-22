"""Host guard and sequencing regressions only. These transcripts are not Android evidence."""

import contextlib
import io
from pathlib import Path
import shlex
import subprocess
import unittest
from unittest.mock import Mock, call, patch

import profile_death_device_test as gate


class Selection(unittest.TestCase):
    def test_requires_explicit_crash_opt_in_and_serial(self):
        with contextlib.redirect_stderr(io.StringIO()):
            for args in ([], ["--disposable-serial", "emulator-5588"], ["--crash-selected-assistant"],
                         ["--disposable-serial", "emulator-5588", "--crash"]):
                with self.subTest(args=args), self.assertRaises(SystemExit):
                    gate.parse_args(args)
        self.assertTrue(gate.parse_args(["--disposable-serial", "emulator-5588",
                                         "--crash-selected-assistant"]).crash_selected_assistant)

    def test_never_contacts_remote_or_implicitly_selected_target(self):
        for serial, env in (("127.0.0.1:5555", {}), ("emulator-5588", {"ADB_SERVER_SOCKET": "tcp:remote:5037"}),
                            ("emulator-5588", {"ANDROID_ADB_SERVER_ADDRESS": "remote"})):
            with (self.subTest(serial=serial, env=env), patch.dict(gate.os.environ, env, clear=True),
                  patch.object(gate.subprocess, "check_output") as cmd, self.assertRaises(SystemExit)):
                gate.select_device(Path("/sdk"), serial)
            cmd.assert_not_called()

    def test_requires_named_avd_then_pins_every_operation_to_its_transport(self):
        for name in ("daily-use", gate.AVD):
            device = "emulator-5588 device product:sdk_gphone64_x86_64 transport_id:17\n"
            with (self.subTest(name=name), patch.dict(gate.os.environ, {}, clear=True),
                  patch.object(gate.subprocess, "check_output", side_effect=[
                      device, f"{name}\nOK\n", "emulator-5588\n", "device\n"]) as cmd):
                if name != gate.AVD:
                    with self.assertRaises(SystemExit):
                        gate.select_device(Path("/sdk"), "emulator-5588")
                    self.assertEqual(cmd.call_count, 2)
                    continue
                gate.select_device(Path("/sdk"), "emulator-5588")("get-state")
                self.assertTrue(all(c.args[0][1:5] == ["-H", "127.0.0.1", "-P", "5037"]
                                    for c in cmd.call_args_list))
                self.assertEqual([c.args[0][5:] for c in cmd.call_args_list], [
                    ["devices", "-l"], ["-s", "emulator-5588", "emu", "avd", "name"],
                    ["-t", "17", "get-serialno"], ["-t", "17", "get-state"]])

    def test_owner_requires_owner_user_matching_debug_apk_unshared_uid_and_selected_role(self):
        owner = [f"package:{gate.TARGET} uid:10123"]
        for replies in (["10"], ["0", "2000"], ["0", "10123", "\n".join(owner * 2)],
                        ["0", "10123", *owner, "other"], ["0", "10123", *owner, gate.TARGET]):
            adb = Mock(side_effect=replies)
            with self.subTest(replies=replies), patch.object(gate, "require_preconfigured_apk") as identity:
                if replies[-1] == gate.TARGET:
                    self.assertEqual(gate.check_owner(adb), 10123)
                    self.assertEqual(adb.call_args_list, [
                        call("shell", "am", "get-current-user"), call("shell", "run-as", gate.TARGET, "id", "-u"),
                        call("shell", "pm", "list", "packages", "-U", "--user", "0", "--uid", "10123"),
                        call("shell", "cmd", "role", "get-role-holders", "--user", "0", "android.app.role.ASSISTANT")])
                else:
                    with self.assertRaises(SystemExit):
                        gate.check_owner(adb)
                self.assertEqual(identity.call_count, int(replies[0] == "0"))


class Evidence(unittest.TestCase):
    def test_stock_exit_filter_accepts_only_the_original_java_crash_record(self):
        identity = "      timestamp=2026-01-15 12:00:01.123 pid=4123 realUid=10123 packageUid=10123 definingUid=10123 user=0\n"
        reason = "      process=com.eon.assistant reason=4 (APP CRASH(EXCEPTION)) subreason=0 (UNKNOWN) status=0\n"
        record = identity + reason
        for text in (record, record.replace("pid=4123", "pid=41230"), record.replace("user=0", "user=10"),
                     record.replace("realUid=10123", "realUid=10124"), record.replace("assistant ", "assistant:extra "),
                     record.replace("4 (APP CRASH(EXCEPTION))", "2 (SIGNALED)"),
                     record.replace("4 (APP CRASH(EXCEPTION))", "5 (APP CRASH(NATIVE))"),
                     identity + "      importance=100\n" + reason):
            def filter_fields(shell, command):
                self.assertEqual(shell, "shell")
                words = shlex.split(command)
                self.assertEqual(words[:-1], ["dumpsys", "activity", "exit-info", gate.TARGET, "|", "sed", "-nE"])
                return subprocess.check_output(words[-3:], input=text, text=True)

            with self.subTest(text=text):
                self.assertEqual(gate.crash_record(filter_fields, 10123, 4123), text == record)

    def test_requires_live_selected_masked_service_and_requested_session_state(self):
        fields = (" mUser=0\n mComponent=" + gate.SERVICE + "\n mDisabledShowContext=3\n"
                  " mBound=true mService=android.service.voice.IVoiceInteractionService$Stub$Proxy@abc\n")
        self.assertTrue(gate.bound(Mock(return_value=fields), shown=False))
        shown = fields + " mShown=true\n"
        self.assertTrue(gate.bound(Mock(return_value=shown), shown=True))
        self.assertFalse(gate.bound(Mock(return_value=shown), shown=False))
        for bad in (fields, shown.replace("=3", "=0"), shown.replace("$Stub$Proxy@abc", ""),
                    shown.replace(gate.SERVICE, "other/.Service"), shown + " mUser=10\n"):
            self.assertFalse(gate.bound(Mock(return_value=bad), shown=True))

    def test_pid_must_belong_to_the_unshared_uid_and_exact_service_process(self):
        app = "app=ProcessRecord{ab12 4123:com.eon.assistant/u0a123}"
        for field in (app, app + "\n" + app, app.replace("u0a123", "u10a123"),
                      app.replace("assistant/", "assistant:extra/"), "app=null"):
            self.assertEqual(gate.service_pid(Mock(return_value=field), 10123), 4123 if field == app else None)

    def test_rebind_without_original_crash_or_with_old_pid_never_passes(self):
        for new, closed, crash in ((None, True, True), (12, True, True), (13, False, True),
                                   (13, True, False), (13, True, True)):
            with (self.subTest(new=new, closed=closed, crash=crash),
                  patch.object(gate, "service_pid", return_value=new),
                  patch.object(gate, "bound", return_value=closed),
                  patch.object(gate, "crash_record", return_value=crash),
                  patch.object(gate.time, "monotonic", side_effect=[0, 1, 2, 61]), patch.object(gate.time, "sleep")):
                if new == 13 and closed and crash:
                    self.assertEqual(gate.replacement(Mock(), 10123, 12), 13)
                else:
                    with self.assertRaises(SystemExit):
                        gate.replacement(Mock(), 10123, 12)

    def test_failure_stops_before_next_phase_and_only_our_observer_is_removed(self):
        crash = ("shell", "am", "crash", "--user", "0", gate.TARGET)
        for stop in ("arm", "changed", "unconfirmed-crash", "rebind", "recover", "pass"):
            def reply(*args, **kwargs):
                if args[:3] == ("shell", "pm", "list"):
                    return "package:" + gate.TARGET
                if args[0] in ("install", "uninstall"):
                    return "Success\n"
                if args == crash:
                    if stop == "unconfirmed-crash":
                        raise gate.subprocess.TimeoutExpired("adb", 15)
                    return ""
                self.assertEqual(args[:8], ("shell", "am", "instrument", "-w", "-r", "--user", "0", "-e"))
                self.assertEqual(args[8::2], ("deathPhase", gate.OBSERVER))
                phase = args[9]
                result = {"arm": "ARMED-visible-action", "recover": "PASS-fresh-consent"}[phase]
                return f"INSTRUMENTATION_RESULT: profileDeath={result if stop != phase else 'PASS-other'}\nINSTRUMENTATION_CODE: -1\n"

            adb = Mock(side_effect=reply)
            with (self.subTest(stop=stop), patch.object(gate, "check_device"),
                  patch.object(gate, "check_owner", return_value=10123), patch.object(gate, "crash_record", return_value=False),
                  patch.object(gate, "service_pid", side_effect=[12, 99 if stop == "changed" else 12, 13]),
                  patch.object(gate, "bound", return_value=True),
                  patch.object(gate, "replacement", side_effect=SystemExit() if stop == "rebind" else None,
                               return_value=13),
                  contextlib.redirect_stdout(io.StringIO()),
                  contextlib.nullcontext() if stop == "pass" else
                  self.assertRaises((SystemExit, gate.subprocess.TimeoutExpired))):
                gate.run(adb, Path("observer.apk"))
            calls = [c.args for c in adb.call_args_list]
            self.assertEqual(calls.count(crash), int(stop not in ("arm", "changed")))
            self.assertEqual([c[9] for c in calls if c[:3] == ("shell", "am", "instrument")],
                             ["arm", "recover"] if stop in ("recover", "pass") else ["arm"])
            self.assertEqual(calls[-1], ("uninstall", gate.TEST))


if __name__ == "__main__":
    unittest.main()
