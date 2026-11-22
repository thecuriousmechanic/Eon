import contextlib
from dataclasses import replace
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import acceptance as bench
import physical_session as physical


class PhysicalInventoryTests(unittest.TestCase):
    """Test inventory/aggregation with analyzer records, not simulated physical WAVs."""

    def setUp(self):
        area = tempfile.TemporaryDirectory()
        self.addCleanup(area.cleanup)
        self.root = Path(area.name)
        (self.root / "trials").mkdir()
        self.samples = {}
        self.info = {"origin": "authorized-single-clock-physical", "route": "unit-test", "mode": "passive",
                     "gains": "unit-test", "threshold_pcm": 500, "sample_rate_hz": 16000, "trials": {}}
        self.add("muted-control.wav", None)
        for i in range(100):
            self.add(f"{i}.wav", physical.SCENES[i % 3])

    def add(self, name, scene):
        path = self.root / ("trials" if scene else "") / name
        path.touch()
        fingerprint = hashlib.sha256(name.encode()).digest()
        self.samples[path] = physical.acoustic.Capture(16000, 8000, 1000, 1016, 2600 if scene else None,
                                                       scene is not None, fingerprint)
        if scene:
            self.info["trials"][name] = {"scene": scene, "pcm_sha256": fingerprint.hex()}
        else:
            self.info["control_pcm_sha256"] = fingerprint.hex()

    def measure(self):
        (self.root / "session.json").write_text(json.dumps(self.info))
        with patch.object(physical.acoustic, "_analyze", side_effect=lambda path, _: self.samples[path]):
            return physical.measure(self.root)

    def test_strict_per_scene_max_and_original_batch_are_independent(self):
        path = self.root / "trials/0.wav"
        self.samples[path] = replace(self.samples[path], first_output_end=4200)  # 200, not <200
        result = self.measure()
        self.assertEqual(result["task_10_2"], {"passed": True, "trials": 100, "p50_ms": 100, "p99_ms": 100})
        self.assertEqual(result["noisy_scenes"]["babble-0db"], {"trials": 34, "max_ttfa_ms": 200, "misses": 0})
        for label in self.info["trials"].values():
            label["scene"] = "control"
        self.assertTrue(all(v["max_ttfa_ms"] is None for v in self.measure()["noisy_scenes"].values()))

    def driven_control(self):
        self.info.update(mode="driven", control_bridge_started=True, control_playback_frame=3360,
                         capture_device="hw:2,0", speaker="alsa:hw:CARD=Test,DEV=1",
                         verified_capture_controller="/dev/snd/by-id/usb-Rig_SN01-00")
        route = {key: self.info[key] for key in ("capture_device", "speaker", "gains", "route", "sample_rate_hz")}
        self.info["route_calibration"] = dict(route, upper_bound_frames=160, reference="unit-test fixture only",
                                              capture_controller=self.info["verified_capture_controller"])

    def test_driven_misses_even_with_speaker_noise_and_started_bridge_without_sound(self):
        self.driven_control()  # exactly 280 ms left after the calibrated bound
        for label in self.info["trials"].values():
            label["bridge_started"] = True
        self.assertTrue(self.measure()["task_10_2"]["passed"])
        self.info["trials"]["0.wav"]["bridge_started"] = False  # analyzer sees a plausible acoustic onset
        path = self.root / "trials/1.wav"
        self.samples[path] = replace(self.samples[path], first_output_end=None)
        measured = self.measure()
        self.assertEqual(measured["task_10_2"], {
            "passed": False, "trials": 100, "p50_ms": None, "p99_ms": None,
            "misses": {"0.wav": "no bridge started", "1.wav": "no sustained speaker response"},
        })
        self.assertEqual(measured["noisy_scenes"][physical.SCENES[0]],
                         {"trials": 34, "misses": 1, "max_ttfa_ms": None})

    def driven_v3(self):
        self.driven_control()
        self.info.update(responder_protocol=3, control_reference_frames=1200, control_captured_before_reference=2500,
                         capture_geometry_verified=True)
        self.info["route_calibration"].update(capture_period_frames=80, capture_buffer_frames=320)
        for trial in self.info["trials"].values():
            trial.update(bridge_started=True, reference_frames=2100, captured_before_reference=2400)

    def test_enqueued_bridge_cannot_explain_an_earlier_acoustic_onset(self):
        self.driven_v3()
        self.assertTrue(self.measure()["task_10_2"]["passed"])
        # Last input at 1016, first sustained speaker bin ends at 2600. Its
        # apparent 100 ms latency cannot come from a bridge enqueued any later.
        trial = self.info["trials"]["0.wav"]
        for already_acquired in (2600, 2601, 8000):
            trial["captured_before_reference"] = already_acquired
            with self.subTest(count=already_acquired), self.assertRaisesRegex(ValueError, "precedes bridge enqueue"):
                self.measure()
        trial["captured_before_reference"] = 2599
        self.assertTrue(self.measure()["task_10_2"]["passed"])  # a straddling bin is not provably premature
        self.info["control_captured_before_reference"] = self.info["control_playback_frame"] + 1
        with self.assertRaisesRegex(ValueError, "control playback observation"):
            self.measure()

    def test_reference_prefix_is_scoped_to_one_versioned_positive_outcome(self):
        self.driven_control()
        for trial in self.info["trials"].values():
            trial["bridge_started"] = True
        self.assertTrue(self.measure()["task_10_2"]["passed"])  # legacy archive
        trial = self.info["trials"]["0.wav"]
        trial["reference_frames"] = 2100
        with self.assertRaisesRegex(ValueError, "version 3"):
            self.measure()
        self.driven_v3()
        trial = self.info["trials"]["0.wav"]
        for invalid in (None, False, 2099, 2100.1, 8001):
            trial["captured_before_reference"] = invalid
            with self.subTest(cutoff=invalid), self.assertRaisesRegex(ValueError, "invalid captured_before_reference"):
                self.measure()
        del trial["captured_before_reference"]
        with self.assertRaisesRegex(ValueError, "invalid captured_before_reference"):
            self.measure()
        trial["captured_before_reference"] = 2400
        for invalid in (None, False, 0, -1, 1.2, "2100", 8001):
            trial["reference_frames"] = invalid
            with self.subTest(count=invalid), self.assertRaisesRegex(ValueError, "invalid reference_frames"):
                self.measure()
        del trial["reference_frames"]
        with self.assertRaisesRegex(ValueError, "invalid reference_frames"):
            self.measure()
        trial.update(bridge_started=False, reference_frames=1999)
        with self.assertRaisesRegex(ValueError, "requires a started"):
            self.measure()
        for version in (True, 3.0, 4):
            self.info["responder_protocol"] = version
            with self.subTest(version=version), self.assertRaisesRegex(ValueError, "invalid responder_protocol"):
                self.measure()

    def test_reference_delivery_during_drain_with_a_silent_speaker_is_a_miss(self):
        self.driven_v3()
        path = self.root / "trials/0.wav"
        self.info["trials"][path.name]["captured_before_reference"] = self.samples[path].frames
        self.samples[path] = replace(self.samples[path], first_output_end=None)
        self.assertEqual(self.measure()["task_10_2"]["misses"], {path.name: "no sustained speaker response"})

    def test_version_three_cannot_omit_verified_capture_geometry(self):
        self.driven_v3()
        del self.info["capture_geometry_verified"]
        with self.assertRaisesRegex(ValueError, "capture_geometry_verified"):
            self.measure()
        self.info["capture_geometry_verified"] = True
        del self.info["route_calibration"]["capture_period_frames"]
        with self.assertRaisesRegex(ValueError, "capture_period_frames"):
            self.measure()

    def test_no_implicit_mode_unknown_outcome_or_unexercised_driven_control(self):
        del self.info["mode"]
        with self.assertRaisesRegex(ValueError, "recording mode"):
            self.measure()
        self.driven_control()
        self.info["control_bridge_started"] = False
        with self.assertRaisesRegex(ValueError, "control did not start"):
            self.measure()
        self.info["control_bridge_started"] = True
        del self.info["control_playback_frame"]
        with self.assertRaisesRegex(ValueError, "live control_playback_frame"):
            self.measure()  # an old driven manifest with just the terminal outcome
        for frame in (None, True, 0, -1, 1.5, "3360", 8001, 3361):
            self.info["control_playback_frame"] = frame
            with self.subTest(frame=frame), self.assertRaisesRegex(ValueError, "live control_playback_frame|captured tail"):
                self.measure()
        self.info["control_playback_frame"] = 3360
        for wrong in (None, 0, 1, "false"):
            if wrong is not None:
                self.info["trials"]["0.wav"]["bridge_started"] = wrong
            with self.assertRaisesRegex(ValueError, "requires boolean bridge_started"):
                self.measure()
        self.info["mode"] = "passive"
        with self.assertRaisesRegex(ValueError, "passive recording cannot declare"):
            self.measure()
        del self.info["control_bridge_started"]
        with self.assertRaisesRegex(ValueError, "passive recording cannot declare control_playback_frame"):
            self.measure()
        del self.info["control_playback_frame"]
        with self.assertRaisesRegex(ValueError, "passive recording cannot declare route_calibration"):
            self.measure()
        del self.info["route_calibration"]
        del self.info["verified_capture_controller"]
        self.info["capture_geometry_verified"] = True
        with self.assertRaisesRegex(ValueError, "passive recording cannot declare capture_geometry_verified"):
            self.measure()
        del self.info["capture_geometry_verified"]
        with self.assertRaisesRegex(ValueError, "passive recording cannot declare"):
            self.measure()

    def test_controller_evidence_is_required_and_must_match_offline(self):
        self.driven_control()
        cal = self.info["route_calibration"]
        del cal["capture_controller"]
        with self.assertRaisesRegex(ValueError, "capture_controller"):
            self.measure()
        cal["capture_controller"] = "/dev/snd/by-path/usb-port"
        self.info["verified_capture_controller"] = cal["capture_controller"]
        with self.assertRaisesRegex(ValueError, "capture_controller"):
            self.measure()
        cal["capture_controller"] = "/dev/snd/by-id/usb-Rig_SN01-00"
        for observed in (None, "/dev/snd/by-id/usb-Rig_SN02-00"):
            self.info["verified_capture_controller"] = observed
            with self.assertRaisesRegex(ValueError, "verified capture controller"):
                self.measure()

    def test_incomplete_or_invalid_declared_capture_geometry_does_not_verify(self):
        self.driven_control()
        cal = self.info["route_calibration"]
        for label in self.info["trials"].values():
            label["bridge_started"] = True
        cal["capture_period_frames"] = 80
        # A legacy recorder preserved arbitrary supplemental calibration notes.
        self.assertTrue(self.measure()["task_10_2"]["passed"])
        for invalid in (None, False, 1, "true"):
            self.info["capture_geometry_verified"] = invalid
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, "invalid capture_geometry_verified"):
                self.measure()
        self.info["capture_geometry_verified"] = True
        with self.assertRaisesRegex(ValueError, "capture_buffer_frames"):
            self.measure()
        cal["capture_buffer_frames"] = 80
        with self.assertRaisesRegex(ValueError, "capture_buffer_frames"):
            self.measure()
        cal["capture_buffer_frames"] = 320
        self.assertTrue(self.measure()["task_10_2"]["passed"])

    def test_driven_control_requires_same_route_positive_calibrated_bound(self):
        self.driven_control()
        cal = self.info.pop("route_calibration")
        with self.assertRaisesRegex(ValueError, "route calibration reference"):
            self.measure()
        for field, invalid in (("reference", [None, " "]), ("upper_bound_frames", [None, False, 0, -1, 1.5, "160"]),
                               ("sample_rate_hz", [8000, 16000.0]), ("speaker", ["other"]), ("capture_device", ["hw:0,0"]),
                               ("route", ["other"]), ("gains", ["other"])):
            for value in invalid:
                self.info["route_calibration"] = dict(cal, **{field: value})
                with self.subTest(field=field, value=value), self.assertRaisesRegex(ValueError, "route calibration"):
                    self.measure()

    def test_any_missing_response_duplicate_rate_change_or_partial_batch_invalidates_result(self):
        path = self.root / "trials/0.wav"
        original = self.samples[path]
        self.samples[path] = replace(original, first_output_end=None)
        self.assertEqual(self.measure()["task_10_2"]["misses"], {"0.wav": "no sustained speaker response"})
        for bad, why in ((replace(original, rate=8000), "rate differs"),
                         (self.samples[self.root / "trials/1.wav"], "duplicate PCM")):
            with self.subTest(why=why):
                self.samples[path] = bad
                self.info["trials"]["0.wav"]["pcm_sha256"] = bad.fingerprint.hex()
                with self.assertRaisesRegex(ValueError, why):
                    self.measure()
        path.unlink()
        with self.assertRaisesRegex(ValueError, "entire trials directory"):
            self.measure()
        del self.info["trials"]["0.wav"]
        with self.assertRaisesRegex(ValueError, "at least 100"):
            self.measure()

    def test_control_integrity_provenance_and_file_type_before_acceptance(self):
        path = self.root / "muted-control.wav"
        original = self.samples[path]
        self.samples[path] = replace(original, any_output=True)
        with self.assertRaisesRegex(ValueError, "muted control exceeds"):
            self.measure()
        self.samples[path] = replace(original, frames=2000)
        with self.assertRaisesRegex(ValueError, "tail"):
            self.measure()
        self.samples[path] = original
        self.info["trials"]["11.wav"]["pcm_sha256"] = "undeclared"
        with self.assertRaisesRegex(ValueError, "differs from declared"):
            self.measure()
        self.info["origin"] = "render-callback"
        with self.assertRaisesRegex(ValueError, "provenance"):
            self.measure()
        path.unlink()
        path.symlink_to(self.root / "trials/1.wav")
        self.info["origin"] = "authorized-single-clock-physical"
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.measure()
        with self.assertRaisesRegex(ValueError, "duplicate manifest key"):
            json.loads('{"trials": {}, "trials": {}}', object_pairs_hook=physical.distinct_keys)

    def test_consistent_wav_headers_do_not_override_the_acquisition_clock(self):
        # PCM digests do not cover the sample rate in a WAV header.
        self.samples = {p: replace(s, rate=8000) for p, s in self.samples.items()}
        with self.assertRaisesRegex(ValueError, "control rate differs"):
            self.measure()

    def test_truncated_reply_window_is_not_a_measured_miss(self):
        path = self.root / "trials/0.wav"
        original = self.samples[path]
        tail = 16 * (physical.acoustic.P99_LIMIT_MS + physical.acoustic.SUSTAINED_OUTPUT_MS)
        missed = replace(original, frames=original.last_input_end + tail, first_output_end=None)
        self.samples[path] = replace(missed, frames=missed.frames - 1)
        with self.assertRaisesRegex(ValueError, "insufficient post-stimulus"):
            self.measure()
        self.samples[path] = missed
        self.assertIn("0.wav", self.measure()["task_10_2"]["misses"])

    def test_malformed_parsers_return_invalid_not_measured_failure(self):
        (self.root / "session.json").write_bytes(b"[" * 2000 + b"]" * 2000)
        # Decoder recursion limits differ among Python builds.
        with (patch.object(physical.json, "loads", side_effect=RecursionError),
              contextlib.redirect_stderr(io.StringIO()) as errors):
            self.assertEqual(physical.main([str(self.root)]), 2)
        self.assertIn("nested too deeply", errors.getvalue())
        (self.root / "session.json").write_text(json.dumps(self.info))
        (self.root / "muted-control.wav").write_bytes(b"RIFF\x0c\0\0\0WAVEJUNK\xff\xff\xff\xff")
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            self.assertEqual(physical.main([str(self.root)]), 2)
        self.assertIn("invalid WAV chunk bounds", errors.getvalue())
        for rate in (None, True, 7999, 192001, "8000"):
            with self.subTest(rate=rate), self.assertRaisesRegex(ValueError, "sample_rate_hz"):
                if rate is None:
                    del self.info["sample_rate_hz"]
                else:
                    self.info["sample_rate_hz"] = rate
                self.measure()

    def test_standalone_device_gate_distinguishes_pass_failure_and_invalid_evidence(self):
        path = self.root / "trials/0.wav"
        def check(exit_code):
            (self.root / "session.json").write_text(json.dumps(self.info))
            with (patch.object(physical.acoustic, "_analyze", side_effect=lambda p, _: self.samples[p]),
                  contextlib.redirect_stdout(io.StringIO()) as out, contextlib.redirect_stderr(io.StringIO()) as err):
                self.assertEqual(physical.main([str(self.root)]), exit_code)
            if exit_code == 2:
                self.assertIn("Invalid physical session", err.getvalue())
                self.assertEqual(out.getvalue(), "")
                return None
            return json.loads(out.getvalue())["task_10_2"]
        self.assertTrue(check(0)["passed"])
        self.samples[path] = replace(self.samples[path], first_output_end=None)
        self.assertEqual(check(1)["misses"], {"0.wav": "no sustained speaker response"})
        del self.info["trials"]["0.wav"]
        check(2)


class AcceptanceTests(unittest.TestCase):
    def test_filtered_renamed_ignored_or_nonzero_kernel_run_is_not_accepted(self):
        ok = "test result: ok. 5 passed; 0 failed; 0 ignored;\n"
        replay = "\n".join(f"test {name} ... ok" for name in bench.REPLAY) + "\n"
        defense = f"test {bench.DEFENSE[0]} ... "
        for text, code, expected in ((ok, 0, ("unavailable", "unavailable")),
                                     (replay + defense + "ignored\n", 0, ("unavailable", "unavailable")),
                                     (replay + ok, 0, ("passed", "unavailable")),
                                     (replay + defense + "FAILED\n", 101, ("passed", "failed")),
                                     (replay + defense + "ok\n" + ok, 1, ("unavailable", "unavailable")),
                                     (replay + defense + "ok\n" + ok, 0, ("passed", "passed"))):
            with (self.subTest(text=text, code=code), contextlib.redirect_stderr(io.StringIO()),
                  patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], code, text)) as child):
                self.assertEqual(tuple(g["status"] for g in bench.kernel_checks()), expected)
                self.assertNotIn("--ignored", child.call_args.args[0])
                command = child.call_args.args[0]
                self.assertEqual(command[command.index("--test") + 1], "eval_components")

    def test_missing_physical_input_is_not_a_software_timing_or_historical_score(self):
        checked = {"status": "passed"}
        with (patch.object(bench, "kernel_checks", return_value=(checked, checked)),
              patch.object(bench.polyglot_repair, "run_corpus", side_effect=bench.BenchError("absent tool")),
              patch.object(bench, "speaker_checks", return_value=checked)):
            report = bench.run(None)
        self.assertIsNone(report["task_10_2"]["p50_ms"])
        self.assertIsNone(report["task_10_2"]["p99_ms"])
        suite = report["task_10_6"]
        self.assertEqual(suite["status"], "unavailable")
        self.assertEqual(suite["gates"]["repair"]["status"], "unavailable")
        self.assertIsNone(suite["gates"]["noisy_physical_ttfa"]["scenes"])

    def test_software_verdict_never_calls_acoustic_analyzer_or_returns_physical_task_claims(self):
        with (patch.object(bench, "common_gates", side_effect=lambda p: {"speaker_decisions": {"status": "passed"}}) as common,
              patch.object(physical, "measure", side_effect=AssertionError),
              patch.object(bench.software_turns, "measure", return_value={"status": "failed"})):
            result = bench.software_run(bench.noisy_duplex.AGREEMENT)
            common.assert_called_once_with(bench.noisy_duplex.AGREEMENT)
        self.assertEqual(set(result), {"scope", "status", "gates"})
        self.assertEqual((result["scope"], result["status"]), ("software-only", "failed"))

    def test_never_combine_physical_sessions_with_software_render_or_experimental_policy(self):
        for argv in (["--software-only", "--physical-session", "/unused"], ["--speaker-policy", bench.noisy_duplex.AGREEMENT]):
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                bench.main(argv)

    def test_every_gate_must_pass_and_200_ms_is_not_below_200(self):
        checked = {"status": "passed"}
        physical_result = {"task_10_2": {"passed": True}, "manifest_sha256": "test", "noisy_scenes": {
            scene: {"max_ttfa_ms": 199, "misses": 0} for scene in physical.SCENES}}
        with (patch.object(bench, "kernel_checks", return_value=(checked, checked)),
              patch.object(bench.polyglot_repair, "run_corpus", return_value={"cases": 50, "passed": True}),
              patch.object(bench, "speaker_checks", return_value=checked) as speaker,
              patch.object(physical, "measure", return_value=physical_result)):
            self.assertEqual(bench.run(None)["task_10_6"]["status"], "passed")
            for value, expected in ((200, "failed"), (None, "unavailable")):
                physical_result["noisy_scenes"][physical.SCENES[1]]["max_ttfa_ms"] = value
                self.assertEqual(bench.run(None)["task_10_6"]["status"], expected)
            physical_result["noisy_scenes"][physical.SCENES[1]]["misses"] = 1
            self.assertEqual(bench.run(None)["task_10_6"]["status"], "failed")
            physical_result["noisy_scenes"][physical.SCENES[1]]["misses"] = 0
            speaker.return_value = {"status": "failed"}
            self.assertEqual(bench.run(None)["task_10_6"]["status"], "failed")

    def test_missing_condition_or_measured_owner_miss_cannot_pass(self):
        rows = [{"condition": c, "duplex_check_passed": True} for c in bench.noisy_duplex.CONDITIONS]
        pairs = [{"pair": p, "status": "passed"} for p in bench.speaker_handoffs.EXPECTED]
        self.enterContext(patch.object(bench.speaker_handoffs, "run_pairs", return_value=pairs))
        with patch.object(bench.noisy_duplex, "run_conditions", return_value=rows[:-1]):
            self.assertEqual(bench.speaker_checks()["status"], "unavailable")
        rows[-1].update(duplex_check_passed=False, owner_missed=True)
        with patch.object(bench.noisy_duplex, "run_conditions", return_value=rows):
            self.assertEqual(bench.speaker_checks()["status"], "failed")

    def test_fixed_scene_success_cannot_hide_a_wrong_reader_or_missing_pair(self):
        rows = [{"condition": c, "duplex_check_passed": True} for c in bench.noisy_duplex.CONDITIONS]
        pairs = [{"pair": p, "status": "passed"} for p in bench.speaker_handoffs.EXPECTED]
        with (patch.object(bench.noisy_duplex, "run_conditions", return_value=rows),
              patch.object(bench.speaker_handoffs, "run_pairs") as native):
            for result, expected in ((pairs, "passed"), (pairs[:-1], "unavailable"),
                                     (pairs[:-1] + pairs[:1], "unavailable"),
                                     (pairs[:-1] + [dict(pairs[-1], status="failed")], "failed")):
                native.return_value = result
                self.assertEqual(bench.speaker_checks()["status"], expected)
            def partial():
                yield dict(pairs[0], status="failed")
                raise OSError("absent corpus")
            native.return_value = partial()
            report = bench.speaker_checks()
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["handoff_input_error"], "absent corpus")
