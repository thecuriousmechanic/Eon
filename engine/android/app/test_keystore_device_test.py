"""Host-side transcript/CLI checks. These tests do not stand in for Android instrumentation."""

import contextlib
import hashlib
import io
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from keystore_device_test import (MIC_PHASES, PROFILE_RESULTS, TARGET, TEST, check_packages, check_result, parse_args,
                                  require_closed_context, require_preconfigured_apk, require_setup_complete,
                                  require_stock_emulator, run)


class InstrumentationResults(unittest.TestCase):
    def test_requires_result_for_the_requested_gate_and_successful_finish(self):
        for key, result in (("keystore", "PASS-issue"), ("keystore", "PASS-restart"),
                            ("demo", "PASS-denied"), ("demo", "PASS-allowed"), ("nativeVault", "PASS"),
                            ("voiceHud", "PASS-stock"), ("voiceHud", "PASS-session"),
                            ("workflowNotice", "PASS-session"),
                            ("controlsMetadata", "PASS-volunteered"),
                            ("controlsMetadata", "PASS-policy-denied"),
                            ("assistantActivity", "PASS-picker-canceled")):
            with self.subTest(key=key, result=result):
                check_result(f"INSTRUMENTATION_RESULT: {key}={result}\r\nINSTRUMENTATION_CODE: -1\r\n", key, result)

    def test_neither_a_stale_pass_nor_am_exit_zero_is_a_completion(self):
        result = "INSTRUMENTATION_RESULT: demo=PASS-denied\n"
        code = "INSTRUMENTATION_CODE: -1\n"
        for output in ("", result, code, result + "INSTRUMENTATION_CODE: 0\n",
                       result.replace("denied", "allowed") + code, result.replace("demo", "keystore") + code,
                       result + result + code, result + code + "INSTRUMENTATION_CODE: 0",
                       "INSTRUMENTATION_FAILED: Process crashed.\n" + result + code):
            with self.subTest(output=output), self.assertRaises(SystemExit):
                check_result(output, "demo", "PASS-denied")

    def test_reports_failure_checkpoint_not_arbitrary_device_output(self):
        with self.assertRaisesRegex(SystemExit, r"^keystore=PASS-restart failed at key-continuity\.$"):
            check_result("INSTRUMENTATION_RESULT: keystore=FAIL-key-continuity\nprovider details\n"
                         "INSTRUMENTATION_CODE: 0\n", "keystore", "PASS-restart")


class DeviceModes(unittest.TestCase):
    def test_profile_case_needs_a_named_policy_and_preconfigured_serial(self):
        with contextlib.redirect_stderr(io.StringIO()):
            for args in (["--serial", "emulator-5554", "--profile-policy", "allowed"],
                         ["--preconfigured-profile-serial", "emulator-5554"]):
                with self.assertRaises(SystemExit):
                    parse_args(args)
        for policy in PROFILE_RESULTS:
            args = parse_args(["--preconfigured-profile-serial", "emulator-5554", "--profile-policy", policy])
            self.assertIsNone(args.serial)
            self.assertEqual(args.profile_policy, policy)

    def test_profile_gate_preserves_target_and_requires_the_requested_policy_result(self):
        for expected in ("PASS-policy-restricted", "PASS-os-action"):
            with (self.subTest(result=expected),
                  patch("keystore_device_test.require_stock_emulator") as emulator,
                  patch("keystore_device_test.require_setup_complete") as setup,
                  patch("keystore_device_test.require_preconfigured_apk") as identity,
                  patch("keystore_device_test.require_closed_context") as closed,
                  patch("keystore_device_test.subprocess.check_output", side_effect=[
                      f"package:android\npackage:{TARGET}\n", "Success\n",
                      f"INSTRUMENTATION_RESULT: profileAction={expected}\nINSTRUMENTATION_CODE: -1\n",
                      "Success\n"]) as adb,
                  contextlib.redirect_stdout(io.StringIO()),
                  contextlib.nullcontext() if expected == PROFILE_RESULTS["restricted"] else self.assertRaises(SystemExit)):
                run(Path("/sdk"), "emulator-5554", Path("/observer.apk"), profile_policy="restricted")
            emulator.assert_called_once()
            self.assertEqual(emulator.call_args.args[1:], ("emulator-5554", 35))
            setup.assert_called_once()
            identity.assert_called_once()
            self.assertEqual(closed.call_count, 2 if expected == PROFILE_RESULTS["restricted"] else 1)
            self.assertEqual([c.args[0][3:] for c in adb.call_args_list], [
                ["shell", "pm", "list", "packages", "-u", "--user", "all"],
                ["install", "-t", Path("/observer.apk")],
                ["shell", "am", "instrument", "-w", "-r", "--user", "current", "-e", "policy", "restricted",
                 f"{TEST}/{TARGET}.ProfileActionAcceptance"], ["uninstall", TEST]])

    def test_operator_observer_has_time_for_all_eight_checkpoints(self):
        calls = []

        def response(cmd, **kwargs):
            args = cmd[3:]
            calls.append((args, kwargs["timeout"]))
            if args[:2] == ["emu", "avd"]:
                return "test\nOK\n"
            if args[:2] == ["shell", "getprop"]:
                return "35\n"
            if args[:3] == ["shell", "pm", "list"]:
                return f"package:android\npackage:{TARGET}\n"
            if args[:3] == ["shell", "am", "instrument"]:
                return "INSTRUMENTATION_RESULT: demo=PASS-allowed\nINSTRUMENTATION_CODE: -1\n"
            self.assertIn(args[0], ("install", "uninstall"))
            return "Success\n"

        with (patch("keystore_device_test.subprocess.check_output", side_effect=response),
              patch("keystore_device_test.require_preconfigured_apk"),
              contextlib.redirect_stdout(io.StringIO())):
            run(Path("/sdk"), "emulator-5554", Path("/test.apk"), preconfigured_allowed=True)
        observation = [limit for args, limit in calls if args[:3] == ["shell", "am", "instrument"]]
        self.assertEqual(len(observation), 1)
        self.assertGreater(observation[0], 8 * 30)
        self.assertTrue(all(limit == 120 for args, limit in calls if args[:3] != ["shell", "am", "instrument"]))

    def test_device_selection_rejects_physical_or_unverified_targets_before_install(self):
        def unexpected(*args):
            self.fail(f"unexpected adb command: {args}")
        with self.assertRaises(SystemExit):
            require_stock_emulator(unexpected, "physical-device", 28)
        def avd(name, api):
            return lambda *args: {("emu", "avd", "name"): name,
                                  ("shell", "getprop", "ro.build.version.sdk"): api}[args]
        for name, api in [("OK", "35"), ("demo\nERROR", "35"), ("demo\nOK", "33"), ("demo\nOK", "")]:
            with self.subTest(name=name, api=api), self.assertRaises(SystemExit):
                require_stock_emulator(avd(name, api), "emulator-5554", 34)
        require_stock_emulator(avd("demo\nOK", "28"), "emulator-5554", 28)

    def test_allowed_is_opt_in_and_separate_from_clean_install(self):
        clean = parse_args(["--serial", "emulator-5554", "--demo-denied"])
        self.assertTrue(clean.demo_denied)
        self.assertIsNone(clean.preconfigured_allowed_serial)
        allowed = parse_args(["--preconfigured-allowed-serial", "emulator-5556"])
        self.assertEqual(allowed.preconfigured_allowed_serial, "emulator-5556")
        self.assertIsNone(allowed.serial)
        self.assertFalse(allowed.demo_denied)
        self.assertFalse(parse_args(["--serial", "emulator-5554"]).demo_denied)

    def test_stock_tone_and_operator_hud_are_distinct_opt_ins(self):
        clean = parse_args(["--serial", "emulator-5554", "--stock-hud", "--demo-denied"])
        self.assertTrue(clean.stock_hud and clean.demo_denied)
        self.assertIsNone(clean.preconfigured_hud_serial)
        manual = parse_args(["--preconfigured-hud-serial", "emulator-5554"])
        self.assertFalse(manual.stock_hud)
        self.assertIsNone(manual.serial)

    def test_selected_context_is_opt_in_and_never_uses_a_stock_target(self):
        manual = parse_args(["--preconfigured-context-serial", "emulator-5556"])
        self.assertEqual(manual.preconfigured_context_serial, "emulator-5556")
        self.assertIsNone(manual.serial)
        self.assertIsNone(parse_args(["--serial", "emulator-5556"]).preconfigured_context_serial)
        self.assertFalse(manual.context_denied)
        self.assertTrue(parse_args(["--preconfigured-context-serial", "emulator-5556", "--context-denied"]).context_denied)

    def test_metadata_observer_requires_both_platform_collection_bits_disabled(self):
        require_closed_context(lambda *args: "  mDisabledShowContext=3\n")
        for field in ("", "mDisabledShowContext=2", "mDisabledShowContext=1",
                      "mDisabledShowContext=0", "mDisabledShowContext=3\nmDisabledShowContext=0"):
            with self.subTest(field=field), self.assertRaises(SystemExit):
                require_closed_context(lambda *args: field)

    def test_workflow_observes_only_its_own_result_and_preserves_operator_target(self):
        manual = parse_args(["--preconfigured-workflow-serial", "emulator-5556"])
        self.assertIsNone(manual.serial)
        serial = manual.preconfigured_workflow_serial
        self.assertEqual(serial, "emulator-5556")
        for key in ("workflowNotice", "voiceHud"):
            calls = []

            def response(cmd, **kwargs):
                self.assertEqual(cmd[1:3], ["-s", serial])
                args = cmd[3:]
                calls.append(args)
                if args == ["shell", "pm", "list", "packages", "-u", "--user", "all"]:
                    return f"package:android\npackage:{TARGET}\n"
                if args == ["install", "-t", Path("/test.apk")] or args == ["uninstall", TEST]:
                    return "Success\n"
                self.assertEqual(args, ["shell", "am", "instrument", "-w", "-r",
                                        f"{TEST}/{TARGET}.WorkflowNoticeAcceptance"])
                self.assertGreater(kwargs["timeout"], 8 * 30)
                return f"INSTRUMENTATION_RESULT: {key}=PASS-session\nINSTRUMENTATION_CODE: -1\n"

            with (self.subTest(key=key),
                  patch("keystore_device_test.subprocess.check_output", side_effect=response),
                  patch("keystore_device_test.require_stock_emulator") as emulator,
                  patch("keystore_device_test.require_setup_complete") as setup,
                  patch("keystore_device_test.require_preconfigured_apk") as identity,
                  contextlib.redirect_stdout(io.StringIO()),
                  contextlib.nullcontext() if key == "workflowNotice" else self.assertRaises(SystemExit)):
                run(Path("/sdk"), serial, Path("/test.apk"), preconfigured_workflow=True)
            self.assertEqual(emulator.call_args.args[1:], (serial, 34))
            setup.assert_called_once()
            identity.assert_called_once()
            self.assertEqual(len(calls), 4)
            self.assertEqual(calls[-1], ["uninstall", TEST])

    def test_selected_session_requires_both_setup_steps_without_changing_settings(self):
        for values in (["0"], ["null"], ["1", "0"], ["1", "null"], ["1", "1"]):
            replies = iter(values)
            calls = []

            def adb(*args):
                calls.append(args)
                return next(replies) + "\n"

            with self.subTest(values=values):
                if values == ["1", "1"]:
                    require_setup_complete(adb)
                else:
                    with self.assertRaisesRegex(SystemExit, "Finish device and current user setup"):
                        require_setup_complete(adb)
                self.assertEqual(calls, [
                    ("shell", "settings", "--user", "current", "get", "global", "device_provisioned"),
                    ("shell", "settings", "--user", "current", "get", "secure", "user_setup_complete"),
                ][:len(values)])

    def test_mic_observes_only_requested_phase_and_leaves_existing_app_alone(self):
        for phase in ("denied", "complete", "stop", "hide", "process-death"):
            with self.subTest(phase=phase):
                parsed = parse_args(["--preconfigured-mic-serial", "emulator-5556", "--mic-phase", phase])
                self.assertEqual(parsed.mic_phase, phase)
                calls = []

                def response(cmd, **kwargs):
                    self.assertEqual(cmd[1:3], ["-s", "emulator-5556"])
                    args = cmd[3:]
                    calls.append(args)
                    if args == ["shell", "pm", "list", "packages", "-u", "--user", "all"]:
                        return f"package:android\npackage:{TARGET}\n"
                    if args[:2] == ["install", "-t"] or args == ["uninstall", TEST]:
                        return "Success\n"
                    self.assertEqual(args, ["shell", "am", "instrument", "-w", "-r", "-e", "mode", phase,
                                            f"{TEST}/{TARGET}.SessionMicAcceptance"])
                    self.assertGreater(kwargs["timeout"], 120)
                    return f"INSTRUMENTATION_RESULT: micHud=PASS-{phase}\nINSTRUMENTATION_CODE: -1\n"

                with (patch("keystore_device_test.subprocess.check_output", side_effect=response),
                      patch("keystore_device_test.require_stock_emulator") as emulator,
                      patch("keystore_device_test.require_setup_complete") as setup,
                      patch("keystore_device_test.require_preconfigured_apk") as identity,
                      contextlib.redirect_stdout(io.StringIO())):
                    run(Path("/sdk"), parsed.preconfigured_mic_serial, Path("/test.apk"), mic_phase=phase)
                self.assertEqual(emulator.call_args.args[1:], ("emulator-5556", 35 if phase == "process-death" else 34))
                setup.assert_called_once()
                identity.assert_called_once()
                self.assertEqual(len(calls), 4)
                self.assertEqual(calls[1], ["install", "-t", Path("/test.apk")])
                self.assertEqual(calls[-1], ["uninstall", TEST])

    def test_digest_is_checked_for_the_current_users_installed_apk(self):
        data = b"target APK fixture"
        remote = "/data/app/eon/base.apk"
        with TemporaryDirectory() as directory:
            apk = Path(directory) / "base.apk"
            apk.write_bytes(data)
            for matches in (True, False):
                def adb(*args):
                    if args == ("shell", "pm", "path", "--user", "current", TARGET):
                        return f"package:{remote}\n"
                    self.assertEqual(args, ("shell", "sha256sum", remote))
                    return f"{hashlib.sha256(data if matches else b'other APK').hexdigest()}  {remote}\n"

                with self.subTest(matches=matches), patch("keystore_device_test.TARGET_APK", apk):
                    if matches:
                        require_preconfigured_apk(adb)
                    else:
                        with self.assertRaisesRegex(SystemExit, "does not match"):
                            require_preconfigured_apk(adb)

    def test_mic_results_cannot_pass_from_another_phase_or_extra_measurement(self):
        for phase in MIC_PHASES:
            with self.subTest(phase=phase):
                output = f"INSTRUMENTATION_RESULT: micHud=PASS-{phase}\nINSTRUMENTATION_CODE: -1"
                check_result(output, "micHud", f"PASS-{phase}")
                with self.assertRaises(SystemExit):
                    check_result("INSTRUMENTATION_RESULT: completion=unvalidated\n" + output,
                                 "micHud", f"PASS-{phase}")
                with self.assertRaises(SystemExit):
                    check_result(output, "micHud", "PASS-other")

    def test_incompatible_requests_are_rejected_not_silently_skipped(self):
        for args in (["--demo-denied"], ["--build-only", "--demo-denied"],
                     ["--preconfigured-allowed-serial", "emulator-5556", "--demo-denied"],
                     ["--preconfigured-allowed-serial", "emulator-5556", "--serial", "emulator-5554"],
                     ["--build-only", "--stock-hud"], ["--preconfigured-hud-serial", "emulator-5554", "--stock-hud"],
                     ["--preconfigured-hud-serial", "emulator-5554", "--demo-denied"],
                     ["--preconfigured-hud-serial", "emulator-5554", "--stock-assistant"],
                     ["--build-only", "--stock-assistant"],
                     ["--preconfigured-mic-serial", "emulator-5554"],
                     ["--serial", "emulator-5554", "--mic-phase", "stop"],
                     ["--build-only", "--mic-phase", "complete"],
                     ["--preconfigured-mic-serial", "emulator-5554", "--mic-phase", "all"],
                     ["--preconfigured-hud-serial", "emulator-5554", "--mic-phase", "complete"],
                     ["--preconfigured-workflow-serial", "emulator-5556", "--mic-phase", "complete"],
                     ["--preconfigured-context-serial", "emulator-5556", "--serial", "emulator-5554"],
                     ["--preconfigured-context-serial", "emulator-5556", "--stock-assistant"],
                     ["--build-only", "--context-denied"], ["--serial", "emulator-5556", "--context-denied"],
                     ["--preconfigured-workflow-serial", "emulator-5556", "--demo-denied"],
                     ["--preconfigured-workflow-serial", "emulator-5556", "--serial", "emulator-5554"],
                     ["--preconfigured-workflow-serial", "emulator-5556", "--preconfigured-hud-serial", "emulator-5556"]):
            with self.subTest(args=args), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                parse_args(args)

    def test_only_the_separate_allowed_gate_accepts_an_installed_target(self):
        clean = "package:android\n"
        target = clean + f"package:{TARGET}\n"
        existing_test = f"package:{TEST}\n"
        check_packages(clean, False)
        check_packages(target, True)
        for inventory, allowed in ((target, False), (clean, True), (clean + existing_test, False),
                                   (target + existing_test, True), ("Error: package manager unavailable\n", False),
                                   (clean + "Error: cannot list packages for user\n", False)):
            with self.subTest(inventory=inventory, allowed=allowed), self.assertRaises(SystemExit):
                check_packages(inventory, allowed)


if __name__ == "__main__":
    unittest.main()
