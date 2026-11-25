import contextlib
import io
import json
import shutil
import struct
import tempfile
import unittest
import wave
from pathlib import Path

from measure_ttfa import latency_ms, main, summarize


class LatencyTests(unittest.TestCase):
    def fixture(self, output_start=200, output_length=15, rate=8000, *, path=None,
                frames=None, level=1800):
        if path is None:
            temp = tempfile.TemporaryDirectory()
            self.addCleanup(temp.cleanup)
            path = Path(temp.name) / "trial.wav"
        with wave.open(str(path), "wb") as wav:
            wav.setparams((2, 2, rate, 0, "NONE", "not compressed"))
            frames = (
                (level * (60 <= i * 1000 // rate < 100),
                 900 * (output_start <= i * 1000 // rate < output_start + output_length))
                for i in range(frames if frames is not None else rate // 4)
            )
            wav.writeframes(b"".join(struct.pack("<hh", *f) for f in frames))
        return path

    def test_two_channels_on_one_clock(self):
        for rate, upper_bound in ((8000, 102), (44100, 103), (48000, 102)):
            with self.subTest(rate=rate):
                # The actual gap is 100ms. Include quantization uncertainty.
                self.assertEqual(latency_ms(self.fixture(rate=rate), 500), upper_bound)

    def test_no_response_single_impulse_overlap_and_truncation_are_not_passes(self):
        for start, duration in [(250, 15), (150, 1), (80, 10)]:
            with self.assertRaises(ValueError):
                latency_ms(self.fixture(start, duration), 500)
        path = self.fixture()
        path.write_bytes(path.read_bytes()[:-4])
        with self.assertRaisesRegex(ValueError, "truncated"):
            latency_ms(path, 500)

    def test_strict_percentile_limits_and_insufficient_sample_size(self):
        with self.assertRaises(ValueError):
            summarize([10] * 99)
        self.assertTrue(summarize([169] * 98 + [279, 500])["passed"])
        self.assertFalse(summarize([170] * 100)["passed"])
        self.assertFalse(summarize([20] * 98 + [280, 281])["passed"])

    def test_renamed_copies_cannot_pad_sample_size(self):
        trial = self.fixture()
        shutil.copyfile(trial, trial.with_name("another.wav"))
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            self.assertEqual(main([str(trial.parent), "--threshold", "500"]), 2)
        self.assertIn("duplicate PCM recording", errors.getvalue())

    def test_partial_final_window_is_not_five_ms_of_output(self):
        with self.assertRaisesRegex(ValueError, "no sustained"):
            latency_ms(self.fixture(frames=8 * 204 + 1), 500)

    def test_wav_only_cli_does_not_bypass_rig_inventory(self):
        trial = self.fixture()
        trials = trial.parent / "trials"
        trials.mkdir()
        trial.rename(trials / trial.name)
        (trial.parent / "session.json").write_text("{}")
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            self.assertEqual(main([str(trials), "--threshold", "500"]), 2)
        self.assertIn("physical_session.py", errors.getvalue())

    def test_muted_control_detects_bleed_short_tail_and_mismatched_clocks(self):
        trial = self.fixture()
        for options, expected in [
            ({"output_start": 70, "output_length": 1, "frames": 3200}, "muted control exceeds"),
            ({"output_length": 0}, "280 ms"),
            ({"output_length": 0, "rate": 16000, "frames": 6400}, "rate differs"),
        ]:
            with self.subTest(options=options):
                control = self.fixture(**options)
                with contextlib.redirect_stderr(io.StringIO()) as errors:
                    self.assertEqual(main([str(trial.parent), "--threshold", "500",
                                           "--muted-control", str(control)]), 2)
                self.assertIn(expected, errors.getvalue())

    def test_acceptance_checks_control_and_all_one_hundred_trials(self):
        trial = self.fixture()
        for i in range(1, 100):
            self.fixture(path=trial.with_name(f"trial-{i}.wav"), level=1800 + i)
        control = self.fixture(output_length=0, frames=3200)
        with contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(main([str(trial.parent), "--threshold", "500",
                                   "--muted-control", str(control)]), 0)
        self.assertEqual(json.loads(output.getvalue()), {
            "trials": 100, "p50_ms": 102, "p99_ms": 102,
            "passed": True, "muted_control_checked": True,
        })

    def test_batch_without_control_still_requires_a_common_sample_rate(self):
        trial = self.fixture()
        self.fixture(path=trial.with_name("second.wav"), rate=44100)
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            self.assertEqual(main([str(trial.parent), "--threshold", "500"]), 2)
        self.assertIn("capture rate differs from first trial", errors.getvalue())
